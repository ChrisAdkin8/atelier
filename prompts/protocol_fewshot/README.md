# Model Protocol few-shot examples

Reference examples the harness ships in the system prompt to prime models for emitting the §2 envelope (`schemas/model_protocol/envelope.v1.json`). The examples cover:

| File | Shape |
|---|---|
| `minimal_edit.md` | Single-file edit, no uncertainty, no `claimed_done` |
| `with_uncertainty.md` | Model raises an `uncertainty` signal instead of acting |
| `completion.md` | Multi-file edit with `claimed_done: true` and `grounding` |

## Per-adapter overrides

Different models respond differently to few-shot priming. §2 specifies a documented hook for per-adapter overrides; the convention is:

```
prompts/protocol_fewshot/
  README.md
  minimal_edit.md          # default
  with_uncertainty.md
  completion.md
  overrides/
    anthropic/             # if exists, used instead of the defaults for Anthropic adapters
    openai/
    ...
```

Override files have the same filenames as the defaults. Missing files fall through to the defaults.

## Validating

Each fenced ```json``` block in these examples is validated against `schemas/model_protocol/envelope.v1.json` by `tests/validate_artifacts.py` (via `FENCED_JSON_RE` + the shared schema registry). Failures show up as `FAIL <file> [fewshot envelope] block N: <reason>` in `make artifacts`.
