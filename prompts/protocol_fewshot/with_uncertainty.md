# Uncertainty raised — example

**User:** Add caching to the `lookup` function.

**Assistant (natural-language reply):**
Before I add caching I need to clarify the invalidation policy. The current `lookup` is called from `api.py`, `worker.py`, and `cli.py` — those have different needs. Three options:

1. TTL — entries expire after N seconds.
2. Explicit invalidation — caller drives eviction.
3. Unbounded — never invalidate (only safe if input domain is small).

Which fits the intent?

**Assistant (envelope):**
```json
{
  "uncertainty": [
    {
      "about": "cache invalidation policy",
      "kind": "ambiguous-spec",
      "asks": "Should cached entries expire on a TTL, drop on explicit invalidation, or be unbounded? The three callers have different needs."
    }
  ],
  "grounding": [
    {"text_span": "lookup is called from api.py, worker.py, cli.py", "source": "tool:grep"}
  ]
}
```

Notes:
- No `claimed_changes` — the assistant did not edit anything this turn.
- The `uncertainty` field surfaces structurally so the UI can render it as a prompt for input, not as prose buried in the conversation.
- `grounding` cites the grep that established the three call sites.
