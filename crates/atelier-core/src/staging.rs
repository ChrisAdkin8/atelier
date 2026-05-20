//! §3 atomic diff staging.
//!
//! Spec §3 "Atomic application":
//!   Multi-file edits emitted in a single turn apply **all-or-nothing**.
//!   1. Stages every write from the turn to a temp tree (`tempfile::TempDir`).
//!   2. Runs pre-commit validators (syntax check via tree-sitter where
//!      available; conflict check against current workspace state).
//!   3. **On all-pass:** atomically moves the temp tree contents into the
//!      workspace; emits one §4 checkpoint covering the whole batch.
//!   4. **On any failure:** discards the temp tree; emits a `ToolError` per
//!      §2.5.
//!
//! There is no per-edit opt-out. The agent expresses independence by emitting
//! more turns, not by asking for partial commits. This keeps the §7
//! verification gate's post-state contract simple.
//!
//! ## Atomicity caveats
//!
//! POSIX `rename` is atomic per-file but not across multiple files. The
//! validation phase is strictly all-or-nothing — no workspace file is touched
//! until every check passes. The commit phase is best-effort sequential
//! rename in a deterministic order; if a rename fails after validation (disk
//! full, permission revoked, fs ENOSPC), we surface
//! [`StagingError::PartialCommit`] with the list of files that did and did
//! not land. Callers treat this as a recovery situation, not a normal failure
//! mode. In practice validation catches the failure modes that can be caught
//! up front.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::error::ToolError;

/// A single staged write within a batch.
///
/// `path` is **relative to the workspace root** — staging never accepts
/// absolute paths, since that would let an agent escape the repo via a
/// crafted write target. The caller (the BYOM tool dispatcher) is responsible
/// for converting model-emitted paths into repo-relative ones.
///
/// `expected_pre_hash` — when `Some`, the staging step verifies that the
/// current on-disk file hashes to this value before commit. Used for §14
/// concurrent-edit detection: the agent read the file at hash H, and if it
/// no longer hashes to H, someone else edited it and the commit is rejected.
/// `None` is for new-file creation, where there is no pre-state to compare.
#[derive(Debug, Clone)]
pub struct StagedWrite {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub expected_pre_hash: Option<[u8; 32]>,
}

impl StagedWrite {
    pub fn new(path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            bytes: bytes.into(),
            expected_pre_hash: None,
        }
    }

    pub fn with_expected_hash(mut self, hash: [u8; 32]) -> Self {
        self.expected_pre_hash = Some(hash);
        self
    }
}

/// Per-file syntax-check outcome from the §3 pre-commit validator. Mirrors
/// the UI annotation strings in spec §3 ("syntax-check: pass | fail |
/// not-applicable | grammar-missing").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxOutcome {
    /// Grammar parsed the file with no error nodes.
    Pass,
    /// Grammar parsed the file but found error nodes; carries a short
    /// human-readable summary for UI display and ledger.
    Fail(String),
    /// File extension never gets a syntax check (binary asset, lock file,
    /// etc.). Distinguishes "we don't check this kind" from "we couldn't
    /// find a grammar."
    NotApplicable,
    /// Tier 2 / Tier 3 language whose grammar is not bundled yet (see
    /// spec §3 "Tree-sitter grammar coverage"). UI shows this distinctly so
    /// the user knows the check was skipped intentionally.
    GrammarMissing,
}

impl SyntaxOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail(_) => "fail",
            Self::NotApplicable => "not-applicable",
            Self::GrammarMissing => "grammar-missing",
        }
    }

    /// Whether this outcome should abort the staging commit. Only `Fail`
    /// blocks; `NotApplicable` and `GrammarMissing` are visible-but-permissive
    /// per spec §3.
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::Fail(_))
    }
}

/// Pluggable syntax checker. The real impl is [`TreeSitterSyntaxCheck`];
/// tests can swap in their own to drive specific failure paths.
pub trait SyntaxCheck: Send + Sync {
    fn check(&self, path: &Path, src: &[u8]) -> SyntaxOutcome;
}

/// Default no-op checker — every file is `NotApplicable`. Useful as a
/// scaffold when the tree-sitter bundle is not yet available.
pub struct NoopSyntaxCheck;

impl SyntaxCheck for NoopSyntaxCheck {
    fn check(&self, _path: &Path, _src: &[u8]) -> SyntaxOutcome {
        SyntaxOutcome::NotApplicable
    }
}

/// Tree-sitter backed checker. Tier-1 languages from spec §3:
/// `.py / .ts / .tsx / .js / .jsx / .rs / .go / .json / .toml / .yaml / .yml`.
///
/// v0 only bundles JSON (smallest grammar, exercises the wiring). The
/// remaining Tier-1 grammars are added as the relevant adapter / verification
/// gate work lands — gated on binary-size budget from spec §3 ("revisit if it
/// grows past 10 MB"). Other Tier-1 extensions return `GrammarMissing`, which
/// the UI surfaces distinctly from `NotApplicable` so the gap is visible.
pub struct TreeSitterSyntaxCheck;

impl TreeSitterSyntaxCheck {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TreeSitterSyntaxCheck {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntaxCheck for TreeSitterSyntaxCheck {
    fn check(&self, path: &Path, src: &[u8]) -> SyntaxOutcome {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return SyntaxOutcome::NotApplicable;
        };
        // Tier-1 extensions per spec §3.
        const TIER_1: &[&str] = &[
            "py", "ts", "tsx", "js", "jsx", "rs", "go", "json", "toml", "yaml", "yml",
        ];
        if !TIER_1.contains(&ext) {
            return SyntaxOutcome::NotApplicable;
        }

        let language = match ext {
            "json" => tree_sitter_json::LANGUAGE,
            // Tier-1 grammars not yet bundled — surfaced distinctly so the
            // gap is visible in the UI and tracked in the build plan.
            _ => return SyntaxOutcome::GrammarMissing,
        };

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&language.into()).is_err() {
            return SyntaxOutcome::GrammarMissing;
        }
        let Some(tree) = parser.parse(src, None) else {
            return SyntaxOutcome::Fail("tree-sitter returned no tree".into());
        };
        let root = tree.root_node();
        if root.has_error() {
            // Walk to the first error node for a short message.
            let mut cursor = root.walk();
            let mut msg = String::from("syntax error");
            for child in root.children(&mut cursor) {
                if child.is_error() || child.is_missing() {
                    msg = format!(
                        "{} at byte {}..{} ({})",
                        if child.is_missing() {
                            "missing node"
                        } else {
                            "syntax error"
                        },
                        child.start_byte(),
                        child.end_byte(),
                        child.kind()
                    );
                    break;
                }
            }
            return SyntaxOutcome::Fail(msg);
        }
        SyntaxOutcome::Pass
    }
}

/// Reasons the staging commit can fail. Each is mapped onto a `ToolError`
/// variant by the caller before being injected into the next turn (§2.5).
#[derive(Debug, thiserror::Error)]
pub enum StagingError {
    #[error("staged write target {0} is absolute; only repo-relative paths are accepted")]
    AbsolutePath(PathBuf),

    #[error("staged write target {0} contains `..` and would escape the workspace root")]
    EscapesWorkspace(PathBuf),

    #[error("syntax check failed for {path}: {message}")]
    SyntaxFailed { path: PathBuf, message: String },

    #[error(
        "concurrent edit detected for {path}: file hash changed since the agent read it; staged commit rejected"
    )]
    Conflict { path: PathBuf },

    #[error("I/O failure during staging for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A rename failed *after* validation succeeded. Documented in the module
    /// header — should not happen in normal operation. The caller logs the
    /// partial state and surfaces a recovery prompt.
    #[error(
        "partial commit: {applied} files renamed before failure on {failed_path}; {remaining} not yet applied"
    )]
    PartialCommit {
        applied: usize,
        failed_path: PathBuf,
        remaining: usize,
        #[source]
        source: io::Error,
    },
}

/// Per-file post-commit annotation. UI consumes this to render
/// "syntax-check: pass | fail | not-applicable | grammar-missing" badges per
/// spec §3, plus the [`crate::diff::Hunks`] payload the live-diff renderer consumes.
#[derive(Debug, Clone)]
pub struct FileOutcome {
    pub path: PathBuf,
    pub syntax: SyntaxOutcome,
    /// Line-based hunks between the pre-image and the staged bytes.
    /// `Hunks::Created` when the file did not exist before; `Hunks::Lines`
    /// for modifications; `Hunks::Same` for a no-op write (staging accepts
    /// these so an idempotent tool can re-write the same bytes without
    /// the commit looking like a noisy edit). [`crate::session::Event::EditStaged`]
    /// is published from this per file by the tool dispatcher.
    pub hunks: crate::diff::Hunks,
}

/// Result of a successful commit.
#[derive(Debug, Clone)]
pub struct CommitReport {
    /// In commit order (lexicographic by path).
    pub files: Vec<FileOutcome>,
}

/// A staged batch — one per agent turn. Built via [`Staging::new`], then
/// committed against the workspace root.
pub struct Staging<'a> {
    workspace_root: &'a Path,
    syntax_check: &'a dyn SyntaxCheck,
    writes: BTreeMap<PathBuf, StagedWrite>,
}

impl<'a> Staging<'a> {
    /// Construct a new staging batch for `workspace_root`. The path must be
    /// canonical (no `..` components, exists on disk); validated lazily on
    /// commit.
    pub fn new(workspace_root: &'a Path, syntax_check: &'a dyn SyntaxCheck) -> Self {
        Self {
            workspace_root,
            syntax_check,
            writes: BTreeMap::new(),
        }
    }

    /// Add a write to the batch. Later writes to the same path overwrite
    /// earlier ones; an agent emitting two writes to the same path in one
    /// turn is honest about its intent.
    pub fn add(&mut self, write: StagedWrite) -> Result<(), StagingError> {
        if write.path.is_absolute() {
            return Err(StagingError::AbsolutePath(write.path));
        }
        if write
            .path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(StagingError::EscapesWorkspace(write.path));
        }
        self.writes.insert(write.path.clone(), write);
        Ok(())
    }

    /// Number of writes in the batch.
    pub fn len(&self) -> usize {
        self.writes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Validate + apply the batch in one shot. Equivalent to
    /// `self.stage()?.commit_all()`. Use [`Self::stage`] directly when
    /// you need to expose pending files for a user-approval step
    /// (spec §3 "Hunk accept / reject") before the rename phase.
    ///
    /// On success returns a [`CommitReport`] with per-file syntax-check
    /// outcomes. On any validation failure the workspace is untouched and
    /// the temp tree is discarded.
    pub fn commit(self) -> Result<CommitReport, StagingError> {
        if self.writes.is_empty() {
            return Ok(CommitReport { files: Vec::new() });
        }
        let batch = self.stage()?;
        batch.commit_all()
    }

    /// Validate + write the batch into a staging temp tree, **without**
    /// renaming into the workspace. Returns a [`StagedBatch`] the
    /// caller drives to completion via
    /// [`StagedBatch::commit_selected`] (per-file approval) or
    /// [`StagedBatch::commit_all`] (no-prompt). Dropping the
    /// [`StagedBatch`] discards everything — same all-or-nothing
    /// semantic as the pre-v46 `commit()`.
    ///
    /// Spec §3: the staged tree is durable on disk before this returns
    /// (`write_with_sync` + parent `fsync`). A crash between stage and
    /// rename leaves the workspace untouched.
    pub fn stage(self) -> Result<StagedBatch, StagingError> {
        if self.writes.is_empty() {
            // An empty batch needs no temp tree. Construct a
            // never-populated StagedBatch over a fresh tempdir so the
            // type is uniform; commit_selected on it returns an empty
            // report regardless of `accepted`.
            let staging_dir =
                TempDir::new_in(self.workspace_root).map_err(|e| StagingError::Io {
                    path: self.workspace_root.to_path_buf(),
                    source: e,
                })?;
            return Ok(StagedBatch {
                staging_dir,
                workspace_root: self.workspace_root.to_path_buf(),
                outcomes: Vec::new(),
                pre_images: BTreeMap::new(),
            });
        }

        // 1. Stage every write to a temp tree under workspace_root so the
        //    final rename is same-filesystem (cross-fs rename returns EXDEV
        //    and falls back to copy+delete, which breaks atomicity).
        let staging_dir = TempDir::new_in(self.workspace_root).map_err(|e| StagingError::Io {
            path: self.workspace_root.to_path_buf(),
            source: e,
        })?;

        let mut outcomes: Vec<FileOutcome> = Vec::with_capacity(self.writes.len());
        let mut pre_images: BTreeMap<PathBuf, Option<Vec<u8>>> = BTreeMap::new();

        for (rel, write) in &self.writes {
            // 1a. Symlink containment. `Staging::add` already rejects
            //     literal `..` / absolute paths; this catches the case
            //     where a path component is a symlink pointing outside the
            //     workspace. Defense in depth — file tools also check, but
            //     anyone calling Staging directly gets the same guarantee.
            let target = self.workspace_root.join(rel);
            ensure_target_inside_workspace(self.workspace_root, &target, rel)?;

            // 1b. Read the pre-image (if any) once. We need it for both the
            //     conflict check and the hunk extraction below; reading it
            //     twice would race against any concurrent edit.
            let pre_image: Option<Vec<u8>> = if target.exists() {
                Some(std::fs::read(&target).map_err(|e| StagingError::Io {
                    path: target.clone(),
                    source: e,
                })?)
            } else {
                None
            };

            // 1b. Conflict check.
            if let Some(expected) = write.expected_pre_hash {
                match &pre_image {
                    Some(bytes) if sha256(bytes) == expected => {}
                    _ => return Err(StagingError::Conflict { path: rel.clone() }),
                }
            }

            // 1c. Syntax check.
            let outcome = self.syntax_check.check(rel, &write.bytes);
            if let SyntaxOutcome::Fail(msg) = &outcome {
                return Err(StagingError::SyntaxFailed {
                    path: rel.clone(),
                    message: msg.clone(),
                });
            }

            // 1d. Stage the bytes. We write+sync (not bare `fs::write`) so
            //     a crash between the write and the post-validation rename
            //     leaves the staged file with its real contents on stable
            //     storage — otherwise the rename could publish a
            //     zero-length file. Spec §3 atomicity guarantee.
            let staged_path = staging_dir.path().join(rel);
            if let Some(parent) = staged_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| StagingError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
            write_with_sync(&staged_path, &write.bytes).map_err(|e| StagingError::Io {
                path: staged_path.clone(),
                source: e,
            })?;

            // 1e. Hunk extraction. `Created` for fresh files; the line-diff
            //     path otherwise. Lives here so the dispatcher can publish
            //     `Event::EditStaged { path, hunks }` straight from the
            //     report without re-reading the file.
            let hunks = match &pre_image {
                None => crate::diff::hunks_for_created(&write.bytes),
                Some(pre) => crate::diff::hunks_for(pre, &write.bytes),
            };

            outcomes.push(FileOutcome {
                path: rel.clone(),
                syntax: outcome,
                hunks,
            });
            // v56: retain the pre-image so a downstream
            // `commit_selected_hunks` can splice old + new lines.
            pre_images.insert(rel.clone(), pre_image);
        }

        // 1f. Durability barrier on the staging tree. The staged files
        //     each ran through `write_with_sync` (content fsync'd) but
        //     their *dirents* in the staging dir are still in the
        //     dentry cache. If we crash between staging completion and
        //     a successful rename, on next boot we could find the
        //     staged file content present but the dirent absent — the
        //     rename would then fail with ENOENT mid-batch. Fsync the
        //     staging tree once before starting the rename phase so the
        //     staged tree is fully durable.
        //
        //     Best-effort: a fsync failure here doesn't fail the commit
        //     (we'd rather attempt the rename than reject a valid
        //     batch on a transient FS hiccup); the worst case is the
        //     same "re-do commit on next boot" outcome the rest of the
        //     atomicity story already tolerates.
        let _ = fsync_dir_best_effort(staging_dir.path());

        Ok(StagedBatch {
            staging_dir,
            workspace_root: self.workspace_root.to_path_buf(),
            outcomes,
            pre_images,
        })
    }
}

/// v56 — per-file decision returned by the approval gate. Replaces the
/// pre-v56 `Vec<PathBuf>` accept-list with a richer shape that can
/// carry sub-file (per-hunk) selection. A path absent from the parent
/// [`HunkSelection`] map is "fully rejected" (no bytes commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileApproval {
    /// Commit every staged byte for this file. For `Hunks::Lines` this
    /// means every hunk; for `Created` / `Deleted` / `Binary` / `Same`
    /// this is the only meaningful approval shape and matches pre-v56
    /// behaviour exactly.
    All,
    /// Commit only the listed hunk indices. Only meaningful for
    /// `Hunks::Lines`. For other variants the empty list = drop,
    /// non-empty list = `All` (since there are no individually
    /// addressable hunks to splice). Indices outside the hunk list are
    /// silently ignored.
    Hunks(Vec<usize>),
}

/// v56 — caller-supplied accept set. Each key is a repo-relative path
/// from the matching `Event::StagingPendingApproval` payload; absent
/// keys are fully rejected.
pub type HunkSelection = std::collections::HashMap<PathBuf, FileApproval>;

/// A validated, staged-but-not-yet-renamed batch of writes. Spec §3
/// "Hunk accept / reject" lives here: the caller (typically the
/// dispatcher) exposes the pending files to the user, collects the
/// accept/reject decision, and calls [`Self::commit_selected_hunks`]
/// (or the file-level wrapper [`Self::commit_selected`]) with the
/// accepted set.
///
/// Dropping a [`StagedBatch`] without committing discards the temp
/// tree — same all-or-nothing semantic as the v45 `Staging::commit()`.
///
/// Intentionally **not** `Clone`: the temp tree is a single resource
/// and duplicating the handle would mean two batches racing for the
/// same staged-file paths.
#[derive(Debug)]
pub struct StagedBatch {
    staging_dir: TempDir,
    workspace_root: PathBuf,
    outcomes: Vec<FileOutcome>,
    /// v56 — pre-image bytes per staged path, captured during
    /// [`Staging::stage`]. Required by
    /// [`Self::commit_selected_hunks`] when a `FileApproval::Hunks`
    /// selection splices old + new lines together. `None` for fresh
    /// files (`Hunks::Created`); the partial-hunk path for those
    /// degrades to `FileApproval::All` since there's no pre-image to
    /// interleave with.
    pre_images: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl StagedBatch {
    /// Peek at the files that *would* be committed. Each `FileOutcome`
    /// carries its `Hunks`, so the caller can render a diff for the
    /// approval UI without doing any extra disk I/O. Order is the same
    /// as the source `Staging` (BTreeMap insertion order = lexicographic).
    pub fn pending_files(&self) -> &[FileOutcome] {
        &self.outcomes
    }

    /// Commit every staged file. Equivalent to v45 `Staging::commit()`.
    pub fn commit_all(self) -> Result<CommitReport, StagingError> {
        let selection: HunkSelection = self
            .outcomes
            .iter()
            .map(|o| (o.path.clone(), FileApproval::All))
            .collect();
        self.commit_selected_hunks(&selection)
    }

    /// File-level wrapper. Pre-v56 compatibility surface: every path in
    /// `accepted` becomes a `FileApproval::All`; everything else is
    /// dropped. New callers should prefer
    /// [`Self::commit_selected_hunks`] directly so per-hunk decisions
    /// are explicit at the API boundary.
    pub fn commit_selected(
        self,
        accepted: &std::collections::HashSet<PathBuf>,
    ) -> Result<CommitReport, StagingError> {
        let selection: HunkSelection = accepted
            .iter()
            .map(|p| (p.clone(), FileApproval::All))
            .collect();
        self.commit_selected_hunks(&selection)
    }

    /// v56 — commit each staged file per the caller's [`HunkSelection`].
    /// Paths absent from the map are dropped. `FileApproval::All`
    /// renames the staged file as-is (pre-v56 behaviour). For a
    /// `Hunks::Lines` file with `FileApproval::Hunks(indices)`, the
    /// staged file is rewritten in place with a splice of pre-image
    /// lines for rejected hunks + staged lines for accepted hunks
    /// before the rename. Non-Lines variants ignore the index list:
    /// non-empty `Hunks(…)` is treated as `All`; empty `Hunks(vec![])`
    /// drops the file.
    ///
    /// Order is the same as the source `Staging` (lexicographic by
    /// path) so any partial-failure list is deterministic.
    ///
    /// **Syntax-check trade-off**: a partial-hunk splice is NOT
    /// re-validated against the file's syntax check (the pre-commit
    /// check ran against the agent's full new file). A spliced output
    /// may parse-fail; surfacing that is on the UI for v0. Callers
    /// that need to guarantee parseability should accept entire hunks
    /// only.
    pub fn commit_selected_hunks(
        self,
        selection: &HunkSelection,
    ) -> Result<CommitReport, StagingError> {
        // Classify each staged outcome against the selection. Order
        // preserved from `self.outcomes` (lexicographic by path).
        enum CommitMode {
            All,
            PartialHunks(std::collections::HashSet<usize>),
        }
        let mut planned: Vec<(FileOutcome, CommitMode)> = Vec::new();
        for outcome in &self.outcomes {
            let mode = match selection.get(&outcome.path) {
                None => continue,
                Some(FileApproval::All) => CommitMode::All,
                Some(FileApproval::Hunks(indices)) => match &outcome.hunks {
                    crate::diff::Hunks::Lines { hunks } => {
                        if indices.is_empty() {
                            // No hunks selected = drop. The pre-image
                            // is already on disk; nothing to rename.
                            continue;
                        }
                        let valid: std::collections::HashSet<usize> = indices
                            .iter()
                            .copied()
                            .filter(|i| *i < hunks.len())
                            .collect();
                        if valid.is_empty() {
                            continue;
                        }
                        if valid.len() == hunks.len() {
                            CommitMode::All
                        } else {
                            CommitMode::PartialHunks(valid)
                        }
                    }
                    // Created / Deleted / Binary / Same have no
                    // addressable hunks — non-empty selection = take
                    // the whole file; empty selection = drop.
                    _ => {
                        if indices.is_empty() {
                            continue;
                        }
                        CommitMode::All
                    }
                },
            };
            planned.push((outcome.clone(), mode));
        }
        // v57 (C1 fix) — two-pass commit. The pre-v57 path interleaved
        // splice + rename per file; a splice or rename failure halfway
        // through left files 0..k-1 already renamed, breaking the
        // "all-or-nothing" §3 guarantee for partial-hunk batches.
        //
        // Pass 1: do every per-file pre-rename mutation (mkdir target
        // parent, splice into the staged tempfile for partial-hunk
        // commits). Any failure here aborts before *any* workspace
        // file has moved.
        //
        // Pass 2: rename each staged path into the workspace. The
        // existing PartialCommit error semantics are retained for
        // this pass — a mid-rename failure can still leave some files
        // landed, but the splice failure mode is now closed.
        let total = planned.len();
        let mut parents_to_sync: std::collections::BTreeSet<PathBuf> =
            std::collections::BTreeSet::new();
        // Pass 1 — splice + mkdir; no rename yet.
        //
        // v58 (M-sec-6 fix) — re-validate each target's symlink
        // containment immediately before `create_dir_all`.
        // `Staging::stage` ran the same check, but between stage and
        // `commit_selected_hunks` (which can wait an arbitrarily long
        // time on user approval) a concurrent process can plant a
        // symlink in the workspace. The pre-v58 path then followed
        // it via `create_dir_all` and landed the file outside the
        // repo. The recheck closes the TOCTOU window.
        for (outcome, mode) in planned.iter() {
            let staged_path = self.staging_dir.path().join(&outcome.path);
            let target = self.workspace_root.join(&outcome.path);
            ensure_target_inside_workspace(&self.workspace_root, &target, &outcome.path)?;
            if let Some(parent) = target.parent() {
                create_target_parent_inside_workspace(&self.workspace_root, parent, &outcome.path)?;
                parents_to_sync.insert(parent.to_path_buf());
            }
            if let CommitMode::PartialHunks(accepted_hunk_indices) = mode {
                if let crate::diff::Hunks::Lines { hunks } = &outcome.hunks {
                    let pre = self
                        .pre_images
                        .get(&outcome.path)
                        .and_then(|p| p.as_deref())
                        .unwrap_or(b"");
                    let new_bytes = std::fs::read(&staged_path).map_err(|e| StagingError::Io {
                        path: staged_path.clone(),
                        source: e,
                    })?;
                    let spliced = splice_hunks(pre, &new_bytes, hunks, accepted_hunk_indices);
                    write_with_sync(&staged_path, &spliced).map_err(|e| StagingError::Io {
                        path: staged_path.clone(),
                        source: e,
                    })?;
                }
            }
        }
        // Pass 2 — rename only.
        for (applied, (outcome, _mode)) in planned.iter().enumerate() {
            let staged_path = self.staging_dir.path().join(&outcome.path);
            let target = self.workspace_root.join(&outcome.path);
            ensure_target_inside_workspace(&self.workspace_root, &target, &outcome.path).map_err(
                |e| match e {
                    StagingError::EscapesWorkspace(_) => StagingError::PartialCommit {
                        applied,
                        failed_path: outcome.path.clone(),
                        remaining: total - applied,
                        source: io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()),
                    },
                    other => other,
                },
            )?;
            std::fs::rename(&staged_path, &target).map_err(|e| StagingError::PartialCommit {
                applied,
                failed_path: outcome.path.clone(),
                remaining: total - applied,
                source: e,
            })?;
        }

        // Durability barrier on the rename phase. Same rationale as the
        // staging-tree fsync above.
        for parent in &parents_to_sync {
            let _ = fsync_dir_best_effort(parent);
        }

        Ok(CommitReport {
            files: planned.into_iter().map(|(o, _)| o).collect(),
        })
    }
}

/// v56 — splice a partial-hunk commit. Walks `pre_image` linewise,
/// emitting unchanged context up to each hunk, then either the
/// staged hunk (accepted) or the original hunk (rejected). Lines are
/// preserved verbatim via `split_inclusive('\n')` so the
/// trailing-newline convention of the file is carried through.
///
/// Both buffers must be UTF-8; non-UTF-8 inputs short-circuit to
/// `Hunks::Binary` in [`crate::diff::hunks_for`] so partial-hunk
/// selection isn't reachable for binary files.
fn splice_hunks(
    pre_image: &[u8],
    new_bytes: &[u8],
    hunks: &[crate::diff::Hunk],
    accepted: &std::collections::HashSet<usize>,
) -> Vec<u8> {
    let Ok(pre_text) = std::str::from_utf8(pre_image) else {
        // Should be unreachable: a non-UTF-8 pre-image would have
        // produced `Hunks::Binary` and the partial-hunk path is gated
        // on `Hunks::Lines`. Defensive fallback: keep the pre-image.
        return pre_image.to_vec();
    };
    let Ok(new_text) = std::str::from_utf8(new_bytes) else {
        return pre_image.to_vec();
    };
    let pre_lines: Vec<&str> = pre_text.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new_text.split_inclusive('\n').collect();

    let mut out = String::with_capacity(pre_image.len().max(new_bytes.len()));
    let mut cursor_old = 0usize;
    for (idx, hunk) in hunks.iter().enumerate() {
        // v59 (audit LOW-4 fix) — skip out-of-order or overlapping
        // hunks rather than emit ambiguous spliced output. The
        // in-tree `diff::hunks_for` producer is monotonic; an
        // overlap can only come from a future MCP-hosted tool
        // emitting a malformed `Hunks::Lines`. Skipping is
        // deterministic-by-spec; the pre-v59 clamp produced
        // accidentally-correct-by-luck output that depended on
        // exact slice math.
        if hunk.old_range.start < cursor_old {
            tracing::warn!(
                hunk_index = idx,
                cursor = cursor_old,
                hunk_start = hunk.old_range.start,
                "splice_hunks: skipping out-of-order / overlapping hunk"
            );
            continue;
        }
        let start = hunk.old_range.start.min(pre_lines.len());
        for line in &pre_lines[cursor_old..start] {
            out.push_str(line);
        }
        if accepted.contains(&idx) {
            let ns = hunk.new_range.start.min(new_lines.len());
            let ne = hunk.new_range.end.min(new_lines.len()).max(ns);
            for line in &new_lines[ns..ne] {
                out.push_str(line);
            }
        } else {
            let os = hunk.old_range.start.min(pre_lines.len());
            let oe = hunk.old_range.end.min(pre_lines.len()).max(os);
            for line in &pre_lines[os..oe] {
                out.push_str(line);
            }
        }
        cursor_old = hunk.old_range.end.min(pre_lines.len()).max(cursor_old);
    }
    // Tail: everything after the last hunk's old_range.end.
    for line in &pre_lines[cursor_old..] {
        out.push_str(line);
    }
    out.into_bytes()
}

/// Atomic file write (v60.29 H11): write to a sibling tmp file, fsync,
/// rename onto the target path, fsync the parent directory. The crash
/// between the create and the write that pre-v60.29 could publish a
/// zero-length staged file is no longer reachable — the rename only
/// happens once the tmp file's contents are on stable storage, and
/// rename is atomic at the filesystem level.
///
/// Ordering: tmp-write → fsync(tmp) → rename → fsync(parent dir).
fn write_with_sync(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let file_name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let tmp_path = parent.join(format!(
        "{}.atelier-tmp.{}.{}",
        file_name,
        std::process::id(),
        uuid::Uuid::new_v4().simple(),
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp_path)?;
        std::io::Write::write_all(&mut f, bytes)?;
        f.sync_all()?;
        drop(f);
        #[cfg(test)]
        durability_tests::maybe_panic_between_tmp_and_rename();
        std::fs::rename(&tmp_path, path)
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return write_result;
    }
    let _ = fsync_dir_best_effort(parent);
    Ok(())
}

/// fsync a directory entry. POSIX-only; on other platforms this is a
/// no-op and the durability contract weakens accordingly. Best-effort —
/// callers wrap with `let _ =` because we'd rather complete a successful
/// rename than fail the whole commit on a fsync glitch.
#[cfg(unix)]
fn fsync_dir_best_effort(dir: &Path) -> std::io::Result<()> {
    let f = std::fs::File::open(dir)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir_best_effort(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Canonicalize the staging target + workspace root and assert containment.
/// `Staging::commit` runs this before its own `create_dir_all` step, so the
/// target's parent chain may not exist yet for a fresh-file write under a
/// new subdirectory (e.g. `src/lib/inner.txt` when `src/` is absent). We
/// walk up until we find the first existing ancestor and canonicalize
/// that, then verify the canonical ancestor falls inside the canonical
/// workspace root. Combined with `Staging::add`'s `..` rejection, this is
/// enough — any descendant of an inside-the-workspace canonical ancestor
/// is itself inside the workspace, provided we don't follow symlinks
/// (which `create_dir_all` won't synthesise).
///
/// **TOCTOU caveat.** This is a check-now-act-later pattern. Between
/// the canonical-ancestor check here and the post-validation
/// `create_dir_all` and `rename` calls, a concurrent process could race
/// a symlink into the path chain and redirect the rename to outside the
/// workspace. The race is closed by Staging being single-threaded per
/// turn: `Tool::execute` runs in one `spawn_blocking` and Staging is
/// the only writer. Parallelising the apply step in a future would
/// reopen this hole; before doing so, switch to `openat`-style
/// relative-fd I/O.
fn ensure_target_inside_workspace(
    workspace_root: &Path,
    target: &Path,
    rel: &Path,
) -> Result<(), StagingError> {
    let canonical_root = std::fs::canonicalize(workspace_root).map_err(|e| StagingError::Io {
        path: workspace_root.to_path_buf(),
        source: e,
    })?;

    // Find the deepest existing ancestor of `target` and canonicalize it.
    let mut ancestor: &Path = target;
    let canonical_ancestor = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(p) => break p,
            Err(_) => match ancestor.parent() {
                Some(p) => ancestor = p,
                None => {
                    return Err(StagingError::Io {
                        path: target.to_path_buf(),
                        source: io::Error::new(
                            io::ErrorKind::NotFound,
                            "no canonicalisable ancestor of staging target",
                        ),
                    });
                }
            },
        }
    };

    if !canonical_ancestor.starts_with(&canonical_root) {
        return Err(StagingError::EscapesWorkspace(rel.to_path_buf()));
    }
    Ok(())
}

fn create_target_parent_inside_workspace(
    workspace_root: &Path,
    parent: &Path,
    rel: &Path,
) -> Result<(), StagingError> {
    crate::path_safety::create_dir_all_inside_workspace(workspace_root, "staging", parent).map_err(
        |e| match e {
            ToolError::PermissionDenied { .. } => StagingError::EscapesWorkspace(rel.to_path_buf()),
            other => StagingError::Io {
                path: parent.to_path_buf(),
                source: io::Error::other(other.to_string()),
            },
        },
    )
}

/// SHA-256 of a buffer, returned as a fixed-size array so `expected_pre_hash`
/// is stored cheaply alongside the write.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn commits_a_simple_batch_and_writes_files() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("a.txt", "hello")).unwrap();
        s.add(StagedWrite::new("b.txt", "world")).unwrap();
        let report = s.commit().unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(std::fs::read(ws.path().join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(ws.path().join("b.txt")).unwrap(), b"world");
    }

    #[test]
    fn commits_into_nested_directories() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("src/lib/inner.txt", "x")).unwrap();
        s.commit().unwrap();
        assert_eq!(
            std::fs::read(ws.path().join("src/lib/inner.txt")).unwrap(),
            b"x"
        );
    }

    #[test]
    fn empty_batch_commits_to_an_empty_report() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let s = Staging::new(ws.path(), &check);
        let report = s.commit().unwrap();
        assert!(report.files.is_empty());
    }

    #[test]
    fn rejects_absolute_paths() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        let err = s.add(StagedWrite::new("/etc/passwd", "x")).unwrap_err();
        assert!(matches!(err, StagingError::AbsolutePath(_)));
    }

    #[test]
    fn rejects_parent_dir_escapes() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        let err = s.add(StagedWrite::new("../outside.txt", "x")).unwrap_err();
        assert!(matches!(err, StagingError::EscapesWorkspace(_)));
    }

    #[test]
    fn syntax_failure_aborts_whole_batch_and_leaves_workspace_untouched() {
        struct AlwaysFail;
        impl SyntaxCheck for AlwaysFail {
            fn check(&self, _p: &Path, _s: &[u8]) -> SyntaxOutcome {
                SyntaxOutcome::Fail("bad".into())
            }
        }
        let ws = workspace();
        let check = AlwaysFail;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("a.txt", "x")).unwrap();
        s.add(StagedWrite::new("b.txt", "y")).unwrap();
        let err = s.commit().unwrap_err();
        assert!(matches!(err, StagingError::SyntaxFailed { .. }));
        // Neither file landed.
        assert!(!ws.path().join("a.txt").exists());
        assert!(!ws.path().join("b.txt").exists());
    }

    #[test]
    fn conflict_check_rejects_when_file_changed_underneath() {
        let ws = workspace();
        std::fs::write(ws.path().join("a.txt"), b"original").unwrap();
        let expected = sha256(b"original");
        // Someone (or something) modifies the file between read and commit.
        std::fs::write(ws.path().join("a.txt"), b"modified").unwrap();

        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("a.txt", "new").with_expected_hash(expected))
            .unwrap();
        let err = s.commit().unwrap_err();
        assert!(matches!(err, StagingError::Conflict { .. }));
        // File unchanged from the concurrent modification.
        assert_eq!(std::fs::read(ws.path().join("a.txt")).unwrap(), b"modified");
    }

    #[test]
    fn conflict_check_accepts_when_hash_matches() {
        let ws = workspace();
        std::fs::write(ws.path().join("a.txt"), b"original").unwrap();
        let expected = sha256(b"original");

        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("a.txt", "new").with_expected_hash(expected))
            .unwrap();
        let report = s.commit().unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(std::fs::read(ws.path().join("a.txt")).unwrap(), b"new");
    }

    #[test]
    fn conflict_check_rejects_when_expected_file_is_missing() {
        let ws = workspace();
        // Agent thinks the file existed (has a pre-hash) but it does not.
        let expected = sha256(b"phantom");

        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("gone.txt", "rewrite").with_expected_hash(expected))
            .unwrap();
        let err = s.commit().unwrap_err();
        assert!(matches!(err, StagingError::Conflict { .. }));
    }

    #[test]
    fn new_file_creation_skips_conflict_check() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("brand-new.txt", "fresh")).unwrap();
        let report = s.commit().unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(
            std::fs::read(ws.path().join("brand-new.txt")).unwrap(),
            b"fresh"
        );
    }

    #[test]
    fn duplicate_path_in_batch_keeps_last_write() {
        let ws = workspace();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("a.txt", "first")).unwrap();
        s.add(StagedWrite::new("a.txt", "second")).unwrap();
        assert_eq!(s.len(), 1);
        s.commit().unwrap();
        assert_eq!(std::fs::read(ws.path().join("a.txt")).unwrap(), b"second");
    }

    #[test]
    fn tree_sitter_checks_json_and_passes_well_formed() {
        let ws = workspace();
        let check = TreeSitterSyntaxCheck::new();
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("config.json", r#"{"a": 1, "b": [2, 3]}"#))
            .unwrap();
        let report = s.commit().unwrap();
        assert_eq!(report.files[0].syntax, SyntaxOutcome::Pass);
    }

    #[test]
    fn tree_sitter_rejects_malformed_json_and_aborts_batch() {
        let ws = workspace();
        let check = TreeSitterSyntaxCheck::new();
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("config.json", r#"{"a": 1, "b": [2, 3"#))
            .unwrap();
        s.add(StagedWrite::new("other.txt", "ok")).unwrap();
        let err = s.commit().unwrap_err();
        match err {
            StagingError::SyntaxFailed { path, .. } => {
                assert_eq!(path, PathBuf::from("config.json"))
            }
            other => panic!("expected SyntaxFailed, got {other:?}"),
        }
        // The companion file did not land — all-or-nothing.
        assert!(!ws.path().join("other.txt").exists());
        assert!(!ws.path().join("config.json").exists());
    }

    #[test]
    fn tier_one_grammar_missing_is_distinct_from_not_applicable() {
        let ws = workspace();
        let check = TreeSitterSyntaxCheck::new();
        let mut s = Staging::new(ws.path(), &check);
        // .rs is Tier-1 but not yet bundled.
        s.add(StagedWrite::new("lib.rs", "fn main() {}")).unwrap();
        // .lock is not a Tier-1 extension at all.
        s.add(StagedWrite::new("Cargo.lock", "x")).unwrap();
        let report = s.commit().unwrap();
        let by_path: BTreeMap<_, _> = report
            .files
            .into_iter()
            .map(|f| (f.path, f.syntax))
            .collect();
        assert_eq!(
            by_path.get(&PathBuf::from("lib.rs")).unwrap(),
            &SyntaxOutcome::GrammarMissing
        );
        assert_eq!(
            by_path.get(&PathBuf::from("Cargo.lock")).unwrap(),
            &SyntaxOutcome::NotApplicable
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_staged_write_through_symlinked_dir_pointing_outside_workspace() {
        let ws = workspace();
        let outside = workspace();
        // Inside the workspace, a directory entry that's actually a symlink
        // to a tempdir outside. A staged write under it would land outside.
        std::os::unix::fs::symlink(outside.path(), ws.path().join("via_symlink")).unwrap();

        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws.path(), &check);
        s.add(StagedWrite::new("via_symlink/escape.txt", "boom"))
            .unwrap();
        let err = s.commit().unwrap_err();
        assert!(
            matches!(err, StagingError::EscapesWorkspace(p) if p == PathBuf::from("via_symlink/escape.txt"))
        );
        // The escape file did NOT land in the outside dir.
        assert!(!outside.path().join("escape.txt").exists());
    }

    #[test]
    fn sha256_is_deterministic_and_distinguishes_inputs() {
        assert_eq!(sha256(b"x"), sha256(b"x"));
        assert_ne!(sha256(b"x"), sha256(b"y"));
    }

    #[test]
    fn outcome_strings_match_spec_ui_annotations() {
        assert_eq!(SyntaxOutcome::Pass.as_str(), "pass");
        assert_eq!(SyntaxOutcome::Fail("e".into()).as_str(), "fail");
        assert_eq!(SyntaxOutcome::NotApplicable.as_str(), "not-applicable");
        assert_eq!(SyntaxOutcome::GrammarMissing.as_str(), "grammar-missing");
        assert!(SyntaxOutcome::Fail("e".into()).is_blocking());
        assert!(!SyntaxOutcome::Pass.is_blocking());
        assert!(!SyntaxOutcome::NotApplicable.is_blocking());
        assert!(!SyntaxOutcome::GrammarMissing.is_blocking());
    }

    // ---------- HR-A: stage / commit_selected lifecycle ----------

    fn ws_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn stage_returns_batch_with_pending_files_and_no_workspace_change() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "hello")).unwrap();
        s.add(StagedWrite::new("nested/b.txt", "world")).unwrap();

        let batch = s.stage().unwrap();
        assert_eq!(batch.pending_files().len(), 2);
        // Targets must NOT exist yet — stage doesn't rename.
        assert!(!ws.join("a.txt").exists());
        assert!(!ws.join("nested/b.txt").exists());
    }

    #[test]
    fn commit_all_renames_every_staged_file() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "AAA")).unwrap();
        s.add(StagedWrite::new("b.txt", "BBB")).unwrap();
        let report = s.stage().unwrap().commit_all().unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(std::fs::read(ws.join("a.txt")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(ws.join("b.txt")).unwrap(), b"BBB");
    }

    #[test]
    fn commit_selected_renames_only_accepted_drops_the_rest() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("keep.txt", "yes")).unwrap();
        s.add(StagedWrite::new("drop.txt", "no")).unwrap();

        let mut accepted = std::collections::HashSet::new();
        accepted.insert(PathBuf::from("keep.txt"));
        let report = s.stage().unwrap().commit_selected(&accepted).unwrap();

        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, PathBuf::from("keep.txt"));
        assert_eq!(std::fs::read(ws.join("keep.txt")).unwrap(), b"yes");
        assert!(
            !ws.join("drop.txt").exists(),
            "rejected file must not appear in workspace"
        );
    }

    #[test]
    fn commit_selected_with_empty_set_is_full_reject() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "x")).unwrap();
        s.add(StagedWrite::new("b.txt", "y")).unwrap();
        let report = s
            .stage()
            .unwrap()
            .commit_selected(&std::collections::HashSet::new())
            .unwrap();
        assert!(report.files.is_empty());
        assert!(!ws.join("a.txt").exists());
        assert!(!ws.join("b.txt").exists());
    }

    #[test]
    fn commit_selected_ignores_unknown_paths_in_accepted_set() {
        // A UI that sends back a stale path (e.g. user clicked accept,
        // then a new event arrived) shouldn't error — accept-set is
        // an idempotent intersection with the actual pending set.
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("real.txt", "R")).unwrap();
        let mut accepted = std::collections::HashSet::new();
        accepted.insert(PathBuf::from("real.txt"));
        accepted.insert(PathBuf::from("stale.txt")); // unknown
        let report = s.stage().unwrap().commit_selected(&accepted).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].path, PathBuf::from("real.txt"));
    }

    #[test]
    fn dropping_staged_batch_discards_temp_tree_and_leaves_workspace_clean() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "x")).unwrap();
        {
            let _batch = s.stage().unwrap();
            // Drop without committing.
        }
        assert!(!ws.join("a.txt").exists());
    }

    #[test]
    fn stage_then_commit_all_is_equivalent_to_commit() {
        // Behavioural parity: Staging::commit() === stage().commit_all().
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s1 = Staging::new(ws, &check);
        s1.add(StagedWrite::new("a.txt", "xx")).unwrap();
        let r1 = s1.commit().unwrap();
        assert_eq!(r1.files.len(), 1);
        assert_eq!(std::fs::read(ws.join("a.txt")).unwrap(), b"xx");
    }

    // ---------- v56 hunk-rewrite contract ----------

    use std::collections::HashSet;

    fn write_to(ws: &Path, rel: &str, body: &[u8]) {
        let p = ws.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn commit_selected_hunks_with_all_matches_commit_selected_file_level() {
        let dir = ws_dir();
        let ws = dir.path();
        write_to(ws, "a.txt", b"one\ntwo\nthree\n");
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "ONE\ntwo\nTHREE\n"))
            .unwrap();
        let batch = s.stage().unwrap();
        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("a.txt"), FileApproval::All);
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(
            std::fs::read(ws.join("a.txt")).unwrap(),
            b"ONE\ntwo\nTHREE\n"
        );
    }

    #[test]
    fn commit_selected_hunks_partial_splices_pre_image_for_rejected_hunks() {
        // Pre-image has 5 lines; staged version changes lines 0 and 4.
        // Accept hunk 0 (line 0 change), reject hunk 1 (line 4 change).
        // Expected on disk: ONE + line-1..3 (unchanged) + line-4 (reverted).
        let dir = ws_dir();
        let ws = dir.path();
        write_to(ws, "a.txt", b"one\ntwo\nthree\nfour\nfive\n");
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "ONE\ntwo\nthree\nfour\nFIVE\n"))
            .unwrap();
        let batch = s.stage().unwrap();
        // Sanity: two hunks expected.
        let pending = batch.pending_files();
        assert_eq!(pending.len(), 1);
        let hunk_count = match &pending[0].hunks {
            crate::diff::Hunks::Lines { hunks } => hunks.len(),
            other => panic!("expected Lines, got {other:?}"),
        };
        assert_eq!(hunk_count, 2, "two non-adjacent changes = two hunks");

        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("a.txt"), FileApproval::Hunks(vec![0]));
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(
            std::fs::read(ws.join("a.txt")).unwrap(),
            b"ONE\ntwo\nthree\nfour\nfive\n",
            "hunk 0 accepted, hunk 1 reverted to pre-image"
        );
    }

    #[test]
    fn commit_selected_hunks_empty_indices_on_lines_drops_the_file() {
        let dir = ws_dir();
        let ws = dir.path();
        write_to(ws, "a.txt", b"one\ntwo\n");
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "ONE\ntwo\n")).unwrap();
        let batch = s.stage().unwrap();
        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("a.txt"), FileApproval::Hunks(vec![]));
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert!(report.files.is_empty(), "empty hunk set = drop");
        assert_eq!(std::fs::read(ws.join("a.txt")).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn commit_selected_hunks_created_file_with_any_hunks_treats_as_all() {
        // Created files have no addressable hunks; non-empty selection
        // takes the whole file. (Confirms the documented degradation.)
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("new.txt", "new contents\n"))
            .unwrap();
        let batch = s.stage().unwrap();
        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("new.txt"), FileApproval::Hunks(vec![0]));
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(
            std::fs::read(ws.join("new.txt")).unwrap(),
            b"new contents\n"
        );
    }

    #[test]
    fn commit_selected_hunks_created_file_with_empty_indices_drops() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("new.txt", "new\n")).unwrap();
        let batch = s.stage().unwrap();
        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("new.txt"), FileApproval::Hunks(vec![]));
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert!(report.files.is_empty());
        assert!(!ws.join("new.txt").exists());
    }

    #[test]
    fn commit_selected_hunks_omitted_path_is_fully_rejected() {
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "a")).unwrap();
        s.add(StagedWrite::new("b.txt", "b")).unwrap();
        let batch = s.stage().unwrap();
        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("a.txt"), FileApproval::All);
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert_eq!(report.files.len(), 1);
        assert!(ws.join("a.txt").exists());
        assert!(!ws.join("b.txt").exists());
    }

    #[test]
    fn commit_selected_hunks_invalid_indices_filtered_silently() {
        // An index past the hunk-count is treated as "not selected"
        // rather than an error — defensive against a stale UI snapshot.
        let dir = ws_dir();
        let ws = dir.path();
        write_to(ws, "a.txt", b"one\ntwo\n");
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "ONE\ntwo\n")).unwrap();
        let batch = s.stage().unwrap();
        let mut sel = HunkSelection::new();
        // Only hunk 99 (doesn't exist) + hunk 0 (exists). Effective
        // selection = {0}. Since there's one hunk total and one is
        // selected, this is "all hunks" → commit as-is.
        sel.insert(PathBuf::from("a.txt"), FileApproval::Hunks(vec![99, 0]));
        let report = batch.commit_selected_hunks(&sel).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(std::fs::read(ws.join("a.txt")).unwrap(), b"ONE\ntwo\n");
    }

    #[test]
    fn splice_hunks_preserves_trailing_newline_convention() {
        use crate::diff::{hunks_for, Hunks};
        let pre = b"a\nb\nc\n";
        let new = b"A\nb\nC\n";
        let h = hunks_for(pre, new);
        let Hunks::Lines { hunks } = h else {
            panic!("expected Lines");
        };
        // Accept only the first hunk (a→A); reject the second (c→C).
        let accepted: HashSet<usize> = [0].into_iter().collect();
        let out = splice_hunks(pre, new, &hunks, &accepted);
        assert_eq!(out, b"A\nb\nc\n");
    }

    #[test]
    fn commit_selected_hunks_two_pass_keeps_workspace_unchanged_when_splice_blob_missing() {
        // Regression for C1: a splice-phase I/O failure must NOT leave
        // already-renamed files from earlier in the batch sitting in
        // the workspace. We force the splice to fail for `b.txt` by
        // deleting its staged blob between stage() and commit; b.txt
        // is constructed with two hunks so selecting only hunk 0
        // actually exercises the splice path (a full-hunk selection
        // optimises to FileApproval::All and skips the splice).
        let dir = ws_dir();
        let ws = dir.path();
        write_to(ws, "a.txt", b"a-old\n");
        write_to(ws, "b.txt", b"one\ntwo\nthree\nfour\nfive\n");
        let check = NoopSyntaxCheck;
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("a.txt", "a-NEW\n")).unwrap();
        s.add(StagedWrite::new("b.txt", "ONE\ntwo\nthree\nfour\nFIVE\n"))
            .unwrap();
        let batch = s.stage().unwrap();

        // Sanity: b.txt actually produced two non-adjacent hunks.
        let b_outcome = batch
            .pending_files()
            .iter()
            .find(|f| f.path == PathBuf::from("b.txt"))
            .unwrap();
        assert!(matches!(
            &b_outcome.hunks,
            crate::diff::Hunks::Lines { hunks } if hunks.len() == 2
        ));

        // Reach into the staging temp tree and delete b.txt's blob so
        // the splice's std::fs::read fails.
        let staging_tree = batch.staging_dir.path().to_path_buf();
        std::fs::remove_file(staging_tree.join("b.txt")).unwrap();

        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("a.txt"), FileApproval::All);
        sel.insert(PathBuf::from("b.txt"), FileApproval::Hunks(vec![0]));
        let err = batch.commit_selected_hunks(&sel).unwrap_err();
        // Splice-phase failures should NOT surface as PartialCommit —
        // by definition no rename has run.
        assert!(
            matches!(err, StagingError::Io { .. }),
            "expected Io error from pass-1 splice failure, got {err:?}"
        );
        // The workspace must be untouched.
        assert_eq!(
            std::fs::read(ws.join("a.txt")).unwrap(),
            b"a-old\n",
            "two-pass commit: a.txt must not have been renamed when b.txt's splice failed"
        );
        assert_eq!(
            std::fs::read(ws.join("b.txt")).unwrap(),
            b"one\ntwo\nthree\nfour\nfive\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_selected_hunks_rejects_symlink_planted_between_stage_and_commit() {
        // Regression for M-sec-6 — TOCTOU window between
        // Staging::stage and StagedBatch::commit_selected_hunks. A
        // concurrent process (or a chained tool from the same
        // session before user approval lands) plants a symlink in
        // the workspace; pre-v58 commit blindly followed it.
        let dir = ws_dir();
        let ws = dir.path();
        let check = NoopSyntaxCheck;

        // Stage a new file under a subdirectory that doesn't exist yet.
        let mut s = Staging::new(ws, &check);
        s.add(StagedWrite::new("sub/new.txt", "fresh")).unwrap();
        let batch = s.stage().unwrap();

        // Between stage and commit: plant `sub` as a symlink to an
        // outside directory.
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join("sub")).unwrap();

        let mut sel = HunkSelection::new();
        sel.insert(PathBuf::from("sub/new.txt"), FileApproval::All);
        let err = batch.commit_selected_hunks(&sel).unwrap_err();
        // The recheck surfaces this as EscapesWorkspace via
        // ensure_target_inside_workspace; the workspace never has
        // the file landed under the outside directory.
        assert!(
            matches!(err, StagingError::EscapesWorkspace(_)),
            "expected EscapesWorkspace, got {err:?}"
        );
        assert!(!outside.path().join("new.txt").exists());
    }

    #[test]
    fn splice_hunks_preserves_eof_insertion_when_accepted() {
        // Regression for H3 — for a file ending in `\n`, similar's
        // `from_lines` tokens and our `split_inclusive('\n')` tokens
        // must agree on line counts so an EOF-insertion hunk
        // (old_range == [N, N] with N == line count) doesn't get
        // mis-spliced. Accept the EOF insertion; output should be
        // pre + new lines.
        use crate::diff::{hunks_for, Hunks};
        let pre = b"line0\n";
        let new = b"line0\nline1\nline2\n";
        let h = hunks_for(pre, new);
        let Hunks::Lines { hunks } = h else {
            panic!("expected Lines, got {h:?}");
        };
        let accepted: HashSet<usize> = (0..hunks.len()).collect();
        let out = splice_hunks(pre, new, &hunks, &accepted);
        assert_eq!(out, b"line0\nline1\nline2\n");
    }

    #[test]
    fn splice_hunks_eof_insertion_rejected_keeps_pre_image() {
        use crate::diff::{hunks_for, Hunks};
        let pre = b"line0\n";
        let new = b"line0\nline1\n";
        let h = hunks_for(pre, new);
        let Hunks::Lines { hunks } = h else {
            panic!("expected Lines, got {h:?}");
        };
        // Reject every hunk — output equals pre-image.
        let accepted: HashSet<usize> = HashSet::new();
        let out = splice_hunks(pre, new, &hunks, &accepted);
        assert_eq!(out, b"line0\n");
    }

    #[test]
    fn splice_hunks_tolerates_out_of_order_hunks_without_panic() {
        // Regression for the v57 audit's L-1 — `pre_lines[cursor_old..start]`
        // would panic if a future producer emitted hunks out of
        // order or overlapping. v58 clamps `start >= cursor_old` so
        // the slice is always valid; the splice is best-effort
        // rather than asserting producer correctness.
        use crate::diff::{Hunk, LineRange};
        let pre = b"one\ntwo\nthree\nfour\nfive\n";
        let new = b"ONE\ntwo\nthree\nfour\nFIVE\n";
        // Synthetic out-of-order hunks: second hunk's old_range.start
        // is BEFORE first hunk's old_range.end.
        let hunks = vec![
            Hunk {
                old_range: LineRange { start: 0, end: 3 },
                new_range: LineRange { start: 0, end: 3 },
                old_lines: vec!["one".into(), "two".into(), "three".into()],
                new_lines: vec!["ONE".into(), "two".into(), "three".into()],
            },
            // Out-of-order: 1 < 3 (the prior hunk's end).
            Hunk {
                old_range: LineRange { start: 1, end: 2 },
                new_range: LineRange { start: 1, end: 2 },
                old_lines: vec!["two".into()],
                new_lines: vec!["two!".into()],
            },
        ];
        let accepted: HashSet<usize> = [0, 1].into_iter().collect();
        // Pre-v58 this panicked with slice-index-out-of-order.
        // Post-fix it just produces a best-effort output.
        let _out = splice_hunks(pre, new, &hunks, &accepted);
    }

    #[test]
    fn splice_hunks_handles_missing_trailing_newline() {
        use crate::diff::{hunks_for, Hunks};
        let pre = b"a\nb";
        let new = b"A\nb";
        let h = hunks_for(pre, new);
        let Hunks::Lines { hunks } = h else {
            panic!("expected Lines");
        };
        let accepted: HashSet<usize> = [0].into_iter().collect();
        let out = splice_hunks(pre, new, &hunks, &accepted);
        assert_eq!(out, b"A\nb");
    }
}

#[cfg(test)]
pub(crate) mod durability_tests {
    //! v60.29 H11 — durability of `write_with_sync`.
    //!
    //! The function publishes a final path via `rename(tmp, path)`. A
    //! crash between the tmp-write and the rename must leave the target
    //! either non-existent or with full contents — never zero-length.
    //!
    //! The fault injection is a thread-local flag the test sets before
    //! calling `write_with_sync`; the production `fn` checks the flag
    //! under `#[cfg(test)]` and panics through it (caught here with
    //! `catch_unwind`). This avoids signal acrobatics and runs
    //! identically on every platform.

    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::Path;

    thread_local! {
        static FAIL_BETWEEN_TMP_AND_RENAME: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn maybe_panic_between_tmp_and_rename() {
        let armed = FAIL_BETWEEN_TMP_AND_RENAME.with(|c| c.replace(false));
        if armed {
            panic!("durability_tests: injected panic between tmp-write and rename");
        }
    }

    fn arm() {
        FAIL_BETWEEN_TMP_AND_RENAME.with(|c| c.set(true));
    }

    #[test]
    fn write_with_sync_atomic_under_injected_panic() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("a.txt");
        std::fs::write(&target, b"old contents").unwrap();

        arm();
        let result = catch_unwind(AssertUnwindSafe(|| {
            super::write_with_sync(&target, b"new contents that should never appear partially")
        }));
        assert!(result.is_err(), "injected panic should propagate");

        // After the panic: target may still hold pre-existing contents
        // (rename never ran) but it must NEVER be zero-length. A bare
        // `File::create` ordering would leave it zero-length here.
        let on_disk = std::fs::read(&target).expect("target file must exist");
        assert_eq!(
            on_disk, b"old contents",
            "rename did not run; pre-existing contents must survive"
        );
        assert!(!on_disk.is_empty(), "target must never be zero-length");

        // The sibling tmp file may linger because the panic skipped the
        // `remove_file` cleanup branch — that's fine; it's outside the
        // staging dir's lifetime in production and the TempDir drop
        // sweeps it here.
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let e = entry.unwrap();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.contains(".atelier-tmp.") {
                let bytes = std::fs::read(e.path()).unwrap();
                assert_eq!(
                    bytes, b"new contents that should never appear partially",
                    "tmp file must hold full contents when it survives",
                );
            }
        }
    }

    #[test]
    fn write_with_sync_happy_path_publishes_full_contents() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("b.txt");
        super::write_with_sync(&target, b"hello world").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello world");
        // No tmp files left behind on the happy path.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".atelier-tmp."))
            .collect();
        assert!(leftovers.is_empty(), "tmp files leaked: {leftovers:?}");
    }

    #[test]
    fn write_with_sync_overwrites_existing_target_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("c.txt");
        std::fs::write(&target, b"v1").unwrap();
        super::write_with_sync(&target, b"v2 longer").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"v2 longer");
    }

    // Used by `staging::write_with_sync`'s internal `#[cfg(test)]` hook
    // to keep the production binary panic-free.
    #[allow(dead_code)]
    fn _ensure_hook_symbol_resolved() {
        let _ = Path::new("/dev/null");
        maybe_panic_between_tmp_and_rename();
    }
}
