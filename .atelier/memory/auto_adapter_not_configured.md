---
name: auto-adapter_not_configured
description: Adapter dependency missing — env var or config file not found.
metadata:
  type: feedback
  auto: true
  created_at: "2026-05-29T19:49:40Z"
---

The adapter refused to construct itself:

```
no queued stream
```

**Likely fix:** the named env var (e.g. `ANTHROPIC_API_KEY`) isn't set. Set it before relaunching the GUI; the adapter is built once at swap-time and won't re-read the environment until the next adapter swap.
