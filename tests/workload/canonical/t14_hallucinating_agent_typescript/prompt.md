# t14 — Hallucinating agent (TypeScript)

This fixture targets the §7 Tier-1 LSP **hallucinated-symbol** detector
(Phase B Track C3 gate). The starting workspace contains a `Foo` class
with one real method. The agent under test will be scripted (by the
`mock_hallucinating_agent_fixture_flagged_within_one_turn_phase_b_seven_gate`
integration test) to add code that calls `foo.nonExistentMethod()`,
where `nonExistentMethod` is not on `Foo`.

The harness's §7 verify pass must emit `Event::VerificationFailed` with
exactly one `Discrepancy::HallucinatedSymbol { symbol:
"nonExistentMethod", lsp_message: contains "does not exist on type 'Foo'" }`
within one turn.

This fixture is **not for live agents** — it's a producer-side test
that proves the Tier-1 mechanical gate fires on the canonical shape.
Live agents are tested through t01 / t02 / t05 / t06 / t10.
