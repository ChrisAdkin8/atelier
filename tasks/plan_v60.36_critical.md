# v60.36 — Critical-severity fixes

**Status:** Scan complete 2026-05-19. **No critical findings.**

Four independent deep-scan passes (atelier-core Rust, atelier-cli Rust, atelier-gui+tui Rust, Python rig + schemas, CI + shell + frontend) produced **zero** items meeting the Critical bar (exploitable today, data loss, or silent correctness bug in landed path the test suite would not catch).

The two structural concerns the CI scan flagged at "High" (nightly workflows committing to `main` from a job whose dependency download is unverified) are real risks but require a supply-chain compromise of a specific transitive dep to fire — they do not constitute exploitable-today criticals. Tracked at high in `plan_v60.36_high.md`.

## Acceptance

- Each scan agent's report contains a "Critical" subsection that is either absent or explicitly "None observed."
- This file remains an empty bucket; if a Critical surfaces from a follow-up scan, append here as `C1`, `C2` … and bump the bundle version to `v60.36.1`.
