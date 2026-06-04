---
name: project_no_silent_provider_fallback
description: Design principle — never auto-fallback to a different provider/model/adapter; any fallback must be an explicit one-click offer the user accepts.
metadata:
  type: project
  verified: 2026-06-04
---

When a configured provider/model is unavailable (endpoint unreachable, auth failure, misconfigured profile), atelier must **fail visibly and offer recovery** — never silently substitute a different adapter, model, or profile.

**Why:** the user asked for a specific model; silently giving them a different one (e.g. Mock, or a 4B local model in place of a 72B) produces misleading results that are worse than a clear failure. Three incidents established this: the GUI's queueless-Mock fallback in `snapshot_current_model` masked real config errors as "no queued stream" (removed v60.98); the `[routing].executor` silent fallback routed tool-result turns through the wrong model (made fail-fast in v60.100 Q3, CLI exit 1 + GUI Err); the silent 8K context-cap fallback looked authoritative when the probe had actually failed (unverified-marker planned in `tasks/plan_provider_health_ux.md`).

**How to apply:** when designing any degraded-mode behaviour (provider down, capability missing, config invalid), the options are (a) fail fast with an actionable message, or (b) surface a one-click *offer* to switch/degrade that the user must accept. Automatic substitution is never an option. Health/reachability state is advisory UI only — it must not gate the run path (a stale "unreachable" verdict must not block a send). See the binding "Design constraints" section of [[plan_provider_health_ux]] and tasks/plan_provider_health_ux.md.
