//! §7 verify Tier-1 LSP client surface (Phase B).
//!
//! Module layout:
//!
//!   - This file — public surface: [`LspInstallOutcome`] + re-exports of
//!     [`approval`].
//!   - [`approval`] — `LspApprovals` first-use approval store. Bit-for-bit
//!     mirror of [`crate::mcp_config::McpApprovals`].
//!
//! What still ships under a `PENDING` spike verdict
//! (`experiments/lsp_spike/`):
//!
//!   - `LspServerHandle`, `launch_typescript_server`, the `lsp/typescript.rs`
//!     diagnostics-to-`Discrepancy` mapper. The spike must establish a GO /
//!     GO-WITH-CAVEATS verdict on `async-lsp 0.2` against
//!     `typescript-language-server` before those land; today the harness +
//!     decision matrix exist (`experiments/lsp_spike/README.md`) but the
//!     verdict is unfilled.
//!
//! The data-layer types in this module (`LspInstallOutcome`, `LspApprovals`)
//! have no `async-lsp` dep — they're consumable today by the GUI/TUI sinks
//! that already carry the `RequestLspInstall` / `LspInstallResolved`
//! variants from v60.22 (Day-0 prep).

pub mod approval;

pub use approval::{lsp_approvals_path, LspApprovals, LSP_APPROVALS_DIR, LSP_APPROVALS_FILE};

/// Outcome of an LSP install attempt — surfaced on the bus via
/// [`crate::session::Event::LspInstallResolved`] so the UI can update the
/// §7 tier indicator (Tier 1 LSP green → Tier 2 tree-sitter yellow on
/// `Declined` / `Failed`).
///
/// Modelled as a tier/fallback ladder per **L-D-3** — same shape as
/// [`crate::verify::VerificationTier`] and
/// [`crate::protocol_strategy::Strategy`]: stable wire labels, agreement
/// test, no auto-promotion arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspInstallOutcome {
    /// Install succeeded; Tier-1 verify can run.
    Installed,
    /// User declined the first-use prompt; harness falls back to
    /// Tier-2 tree-sitter (or Tier-3 textual) verify for this language.
    Declined,
    /// LSP server already present on the system; no install needed.
    AlreadyPresent,
    /// Install failed (sandboxed subprocess error, network failure,
    /// package-manager not available). Falls back to Tier 2/3 the same
    /// way `Declined` does, but logs distinctly for triage.
    Failed,
}

impl LspInstallOutcome {
    /// Stable wire label used by the GUI bridge, TUI projection, and the
    /// `schemas/audit/lsp_install.v1.json` audit row. Pinned by
    /// `lsp_install_outcome_wire_labels_are_stable` so a future variant
    /// rename forces a deliberate edit on the wire side.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Declined => "declined",
            Self::AlreadyPresent => "already_present",
            Self::Failed => "failed",
        }
    }

    /// Whether Tier-1 verify can run for the language this outcome
    /// describes. `Installed` / `AlreadyPresent` are the only two
    /// outcomes that admit Tier 1; `Declined` and `Failed` fall back to
    /// Tier 2/3.
    pub fn tier_one_available(self) -> bool {
        matches!(self, Self::Installed | Self::AlreadyPresent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_install_outcome_wire_labels_are_stable() {
        assert_eq!(LspInstallOutcome::Installed.wire_label(), "installed");
        assert_eq!(LspInstallOutcome::Declined.wire_label(), "declined");
        assert_eq!(
            LspInstallOutcome::AlreadyPresent.wire_label(),
            "already_present"
        );
        assert_eq!(LspInstallOutcome::Failed.wire_label(), "failed");
    }

    #[test]
    fn tier_one_available_only_when_installed_or_already_present() {
        assert!(LspInstallOutcome::Installed.tier_one_available());
        assert!(LspInstallOutcome::AlreadyPresent.tier_one_available());
        assert!(!LspInstallOutcome::Declined.tier_one_available());
        assert!(!LspInstallOutcome::Failed.tier_one_available());
    }
}
