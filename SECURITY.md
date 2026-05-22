# Security policy

Atelier touches several security-sensitive surfaces: it stores credentials (via `keyring`), runs shell commands inside a sandbox (§11), connects to remote model providers and MCP servers, and ships a curated catalog of third-party servers. This document explains how to report security issues and what response you can expect.

## Reporting a vulnerability

**Do not file a public GitHub issue for security reports.** Use one of these channels instead:

- **GitHub private vulnerability reporting** (preferred) — use `Security` → `Report a vulnerability` on this repository to open a private report with the maintainers.

No security email is published until a monitored project address is available.

Include, where possible:

- A description of the issue and its impact (confidentiality / integrity / availability).
- Steps to reproduce, with a minimal canonical-workload task (t01–t11) or session artifact where applicable.
- Atelier version (`CHANGELOG.md` head or git SHA) and reference machine.
- Whether you've discussed the issue with anyone else.

## Response SLOs

| Phase | Target |
|---|---|
| Acknowledge receipt | ≤ 3 business days |
| Initial assessment (severity + reproduction) | ≤ 10 business days |
| Public disclosure (after patch ships) | ≤ 90 days from initial report, coordinated with reporter |

Critical issues (RCE, credential exfiltration, sandbox escape) are prioritized over the SLOs above and may ship out-of-band releases.

## Supported versions

Until v0.1 ships, **no version is "supported"** in the security-fix sense. The harness is runnable but pre-release; security reports against the spec, schemas, rig, or implementation are welcomed and will be triaged on the same SLOs.

Once v0.1 ships, the supported-version policy is:

- The latest minor release receives security fixes for at least 6 months.
- Older minor releases are supported on a best-effort basis until the next minor lands.

## Scope

In scope:

- The spec (`coding-harness-spec.md`) — design-level security issues, e.g., a way to escape §11 sandboxing or bypass §8 trust budgets.
- The schemas — issues where a schema fails to constrain something it should.
- The rig (`validate_*.py`, runner, fixtures) — issues where the rig itself can be exploited.
- The harness once shipped — `atelier-core`, `atelier-gui`, `atelier-tui`, including credential storage, MCP-client behavior, sandbox enforcement.
- Bundled MCP catalog entries — if a recommended server is itself a vulnerability vector.

Out of scope:

- Vulnerabilities in third-party MCP servers Atelier registers but does not bundle. Report those to the server's maintainer; we will coordinate disclosure when reasonable.
- Vulnerabilities in the underlying LLM providers (Anthropic, OpenAI, etc.).
- Theoretical attacks requiring the user to opt out of every default safeguard.

## Hardening expectations for users

- Run Atelier inside the §11 sandbox defaults; do not set `allow_net: true` on tools unless required.
- Keep `mcp_servers.json` under version control if you want auditability; review additions before invoking.
- Use provider `api_key = "keyring:SERVICE/USER"` references for LLM credentials where possible; `env:NAME` remains available for CI and one-off runs. Avoid plaintext literals in committed manifests. `${keychain:…}` interpolation in MCP manifests is still reserved and fails closed.
- Enable `--local-only` mode for sensitive sessions (§12).
- Review the egress audit log (`schemas/audit/egress.v1.json`) periodically.

## Credits

Reporters of valid issues are credited in the relevant release's CHANGELOG entry unless they opt out.
