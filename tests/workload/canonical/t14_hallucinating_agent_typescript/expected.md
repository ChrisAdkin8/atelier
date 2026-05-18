# t14 — expected outcome

The mock-scripted "hallucinating agent" writes a TypeScript file whose
post-edit content calls a method `Foo` does not declare. The §7
verify pass must:

1. Emit `Event::VerificationFailed { tier: Tier1Lsp, discrepancies }`
   exactly once on the first turn (no `VerificationPassed`).
2. `discrepancies` must contain exactly one `HallucinatedSymbol`
   with `symbol == "nonExistentMethod"` and `lsp_message` quoting
   `does not exist on type 'Foo'`.
3. The v60.12 `mock_lying_agent_fixture_flagged_within_one_turn_phase_a_seven_gate`
   must still pass (no regression on the Tier-3 textual gate).

The fixture also pins the **L-D-9 priority lattice**: a turn that fires
both Tier-1 and Tier-3 produces all matching discrepancies, with the
`tier` badge set to `Tier1Lsp` per the highest-tier-wins rule. A
sibling unit test in `crate::verify` exercises this directly via
`VerificationRun::merged_tier1_lsp`.
