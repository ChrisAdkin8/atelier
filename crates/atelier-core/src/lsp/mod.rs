//! §7 verify Tier-1 LSP client surface (Phase B).
//!
//! Day-0 prep state: only the [`LspInstallOutcome`] enum lives here so the
//! new `Event::RequestLspInstall` / `Event::LspInstallResolved` variants
//! can land on the bus with the four UI sinks updated in lockstep
//! (per **L-D-2** the variant prep is its own commit so the four parallel
//! Phase B bundles don't collide on `session.rs`).
//!
//! The fleshed-out client (`LspServerHandle`, `launch_typescript_server`,
//! `LspApprovals`) lands in Phase B Track C1; the Tier-1 verify producer in
//! `crate::verify` lands in Track C2; the hallucinating-agent fixture in
//! Track C3.

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
    /// Stable wire label used by the GUI bridge and TUI projection.
    /// Pinned by `lsp_install_outcome_wire_labels_are_stable` so a future
    /// variant rename forces a deliberate edit on the wire side.
    pub fn wire_label(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Declined => "declined",
            Self::AlreadyPresent => "already_present",
            Self::Failed => "failed",
        }
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
}
