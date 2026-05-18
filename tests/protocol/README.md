# `tests/protocol/`

Two things live here:

1. `overhead.json` — the §2 protocol-overhead report, written by `atelier protocol-overhead` and refreshed by the nightly CI job (`.github/workflows/nightly_protocol_overhead.yml`). Schema: `schemas/protocol/overhead.v1.json`. Validated by `tests/validate_artifacts.py` on every PR.
2. `fixtures/` — scripted MockAdapter inputs the harness round-trips against each emission strategy. Format documented below.

## `fixtures/<strategy>.json` — format

Each file is a JSON array of `OverheadFixture` entries:

```json
[
  {
    "label": "single_edit_with_grounding",
    "envelope": { ... a Model Protocol Envelope ... },
    "round_trip_lossless": true
  }
]
```

Fields:

- `label` — short human-readable name. Surfaces in log lines and regression alerts. Stay snake_case so it greps cleanly.
- `envelope` — the envelope to encode + decode under the strategy. Must round-trip serde and pass `Envelope::validate` (the harness rejects malformed fixtures up front, so a typo here fails loudly instead of silently producing a zero-byte measurement).
- `round_trip_lossless` — defaults to `true`. Set `false` for fixtures that intentionally exercise the lossy `regex_prose` path (e.g., use `plan_update` or `constraints_acknowledged`). Lossy fixtures contribute to `bytes_on_wire` / `tokens_envelope` / `parse_time_ns` but do **not** count against `conformance_rate`.

The filename picks the strategy: `native_tool.json` → `Strategy::NativeTool`, `json_sentinel.json` → `Strategy::JsonSentinel`, `regex_prose.json` → `Strategy::RegexProse`. Missing files are silently skipped (the harness errors if *all three* are absent). Run only what you have fixtures for.

## Reusing the fixtures from adapter tests

The fixture format is intentionally minimal so future adapter tests can re-use it without writing a sibling format. The load primitive is one line:

```rust
let fixtures: Vec<atelier_cli::overhead::OverheadFixture> =
    serde_json::from_reader(std::fs::File::open(path)?)?;
```

Each entry carries everything an adapter test needs to script the response (the envelope) plus the label for assertion messages. Adapter integration tests that want to drive a mock provider with these envelopes wrap each entry's `envelope` in a `MockResponse` via `runner::mock_envelope_tool_call` (see `crates/atelier-cli/tests/run_integration.rs` for a template).

## Running the harness

Locally:

```sh
cargo run -p atelier-cli -- protocol-overhead
```

By default the harness reads `tests/protocol/fixtures/` and writes `tests/protocol/overhead.json` relative to the current directory. Flags:

- `--workspace <PATH>` — project root override.
- `--fixtures-dir <PATH>` / `--out <PATH>` — explicit overrides.
- `--check-regression` — compare against the prior `rolling_median.value` and exit non-zero (code 3) on drift > 10%. The output file is rewritten regardless so the next nightly's baseline is current.
- `--regression-threshold-pct <N>` — drift percentage that fails the check. Default 10.0.

## Schema mapping note

The strategy enum in `schemas/protocol/overhead.v1.json` is `["native_tool", "json_sentinel", "regex_prose"]` — matching `Strategy::as_str()` 1:1 since v60.28 H16 fixed the `json_mode` typo.
