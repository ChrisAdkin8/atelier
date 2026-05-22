---
name: auto-provider_unreachable
description: Provider's HTTP endpoint did not respond.
metadata:
  type: feedback
  auto: true
  created_at: "2026-05-21T17:42:41Z"
---

The adapter could not reach the configured `base_url`:

```
error sending request for url (http://atelier-gpu-vllm-dev-1460977764.us-east-1.elb.amazonaws.com/v1/chat/completions)
```

**Likely fix:**
- For a local server: confirm it's running. On macOS, `lsof -i :8080 -sTCP:LISTEN` (or whatever port your `providers.toml` uses) should show a Python / llama-server process.
- For mlx-lm specifically: `mlx_lm.server --model <id> --host 127.0.0.1 --port 8080 --chat-template-args '{"enable_thinking": false}'`.
- For a cloud provider: check network / VPN / corporate proxy.
