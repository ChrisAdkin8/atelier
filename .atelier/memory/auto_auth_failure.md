---
name: auto-auth_failure
description: Adapter auth failed — credentials missing or expired.
metadata:
  type: feedback
  auto: true
  created_at: "2026-05-22T07:59:57Z"
---

The adapter rejected the request with an authentication error:

```
{"error":"Unauthorized"}
```

**Likely fix:**
- For Anthropic: export `ANTHROPIC_API_KEY` (in `~/.envrc` for direnv users, or your shell rc).
- For OpenAI / openai-compat against OpenAI itself: export `OPENAI_API_KEY`.
- For a local server (mlx_lm.server, Ollama, vLLM): the server usually doesn't need a key; if it returned 401, check the server config rather than the env var.

**How to verify:** rerun any prompt — a successful round-trip clears the issue.
