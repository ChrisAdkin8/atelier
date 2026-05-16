# Atelier version compatibility matrix

Three independent version streams. This table records which combinations are supported.

| Atelier release | Spec version | Session schema | Protocol envelope |
|---|---|---|---|
| 0.1.x | spec.md @ initial commit | session v1 | envelope v1 |

### Rules
- A new envelope version requires a session-schema bump (sessions embed envelopes).
- A new session-schema version requires a migration tool from N to N+1.
- Atelier reads its own session version and older; it does not auto-upgrade to a future version's schema.
- Replay across major session versions is best-effort; the migration tool emits a compatibility report.
