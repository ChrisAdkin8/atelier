# Reference machine spec

Performance budgets in `coding-harness-spec.md` (cross-cutting → Performance budgets) and the baseline measurement procedure in `tests/workload/canonical/baseline_procedure.md` are evaluated against the machine characterized here.

## Hardware

| Field | Value |
|---|---|
| Model | MacBook Pro (`MacBookPro18,1`) |
| Chip | Apple M1 Pro — 10 cores (8 Performance + 2 Efficiency) |
| RAM | 32 GB |
| Disk | 926 GB internal SSD; ≥250 GB free required during benchmark runs |
| GPU | Apple M1 Pro integrated (no discrete GPU) |

## Software

| Field | Value |
|---|---|
| OS | macOS 26.4.1 (build `25E253`) |
| Shell | zsh (`/bin/zsh`) |
| Python | 3.14.4 |
| Node | v25.8.2 |
| Network | wired or stable Wi-Fi via `en0`; no VPN during benchmark runs |

## Benchmark protocol

- Quit other apps before runs (especially browsers, IDE language servers, communication clients).
- Disable Spotlight indexing of the workdir (`mdutil -i off <path>`).
- Plug in the power adapter; do not run on battery.
- Run each workload **3 times**; report the median.
- Allow 30 seconds idle between runs to let any background indexing settle.

## Versioning

This file is the source of truth for what "the reference machine" means. When the machine changes meaningfully — major OS upgrade, RAM swap, different chip — bump the date stamp below and re-capture every baseline that referenced the prior spec (especially `tests/baselines/permission_prompts.json`).

Last updated: 2026-05-16
