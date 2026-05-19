#!/usr/bin/env python3
"""Shai-Hulud / npm supply-chain IoC sweep.

Three sub-second checks that catch the worm's mechanical footholds before
they can land on `main`. See `tasks/shai_hulud_sweep_2026-05-19.md` for the
incident background.

Checks:
  1. The IoC GitHub Actions file (`shai-hulud-workflow.yml`) is not
     present anywhere in the tree.
  2. No npm lockfile entry declares a `preinstall` or `postinstall`
     lifecycle script. The Shai-Hulud worm's only propagation path is
     a `postinstall` that downloads its payload; banning install hooks
     repo-wide removes the entry point.
  3. Every `resolved` URL in the npm lockfile points at
     `registry.npmjs.org`. A `git+`, `file:`, `http://`, or
     attacker-host tarball reference is a classic dependency-confusion
     red flag.

Operates on `crates/atelier-gui/ui/package-lock.json` (the only
npm-consuming directory in the workspace). Adapts cleanly if more npm
trees land — see `LOCKFILE_PATHS` below.

Exits 0 when clean, 1 with a per-check explanation when any IoC fires.
Designed for `make audit` and the per-PR CI `audit` job.

Run as `python3 scripts/npm_ioc_sweep.py [--repo-root PATH]`.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# Banned GH Actions file. Sweep is recursive — any path in the tree.
SHAI_HULUD_WORKFLOW = "shai-hulud-workflow.yml"

# Every package-lock.json in the workspace gets checked. Add more as
# additional npm trees show up.
LOCKFILE_PATHS = [
    "crates/atelier-gui/ui/package-lock.json",
]

# Allow-list of resolved tarball hosts. Anything outside this set is a
# red flag — Shai-Hulud's secondary propagation in some variants
# substituted git+ or http URLs.
ALLOWED_RESOLVED_HOSTS = frozenset({"registry.npmjs.org"})

# Banned lifecycle script keys. Pre/post install run untrusted code
# during `npm install`; that's the worm's primary entry point.
BANNED_LIFECYCLE_SCRIPTS = frozenset({"preinstall", "postinstall"})


def check_no_shai_hulud_workflow(repo_root: Path) -> list[str]:
    """Check 1: no `shai-hulud-workflow.yml` anywhere in the tree."""
    offenders: list[str] = []
    # Walk the tree but skip well-known caches that aren't part of
    # tracked source (`.git`, `target`, `node_modules`, `__pycache__`).
    skip = {".git", "target", "node_modules", "__pycache__", ".venv"}
    for path in repo_root.rglob(SHAI_HULUD_WORKFLOW):
        if any(part in skip for part in path.parts):
            continue
        offenders.append(str(path.relative_to(repo_root)))
    return offenders


def _shown(lockfile: Path) -> str:
    """Render a lockfile path for diagnostic messages — relative to the
    repo root when possible, absolute otherwise (the test-only path
    where the lockfile lives outside REPO_ROOT)."""
    try:
        return str(lockfile.relative_to(REPO_ROOT))
    except ValueError:
        return str(lockfile)


def check_no_lifecycle_scripts(lockfile: Path) -> list[str]:
    """Check 2: no dependency declares a `preinstall` / `postinstall` script.

    npm v3+ lockfiles encode per-package scripts under
    `packages.<key>.scripts.{preinstall,postinstall}`. We walk the whole
    `packages` map (npm lockfile v3 schema) so transitive deps are
    covered too.
    """
    if not lockfile.is_file():
        return []  # No lockfile → nothing to check; the workspace just doesn't use npm here.
    try:
        doc = json.loads(lockfile.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        return [f"{_shown(lockfile)}: lockfile is not valid JSON: {e}"]

    offenders: list[str] = []
    packages = doc.get("packages") or {}
    for pkg_key, pkg in packages.items():
        if not isinstance(pkg, dict):
            continue
        scripts = pkg.get("scripts") or {}
        for banned in BANNED_LIFECYCLE_SCRIPTS:
            if banned in scripts:
                # Top-level package (key == "") is the workspace itself;
                # surface the path differently to make the message
                # readable.
                shown_key = pkg_key if pkg_key else "<workspace-root>"
                offenders.append(
                    f"{_shown(lockfile)}: `{shown_key}` declares "
                    f"`scripts.{banned}` = {scripts[banned]!r}"
                )
    return offenders


def check_lockfile_hosts(lockfile: Path) -> list[str]:
    """Check 3: every `resolved` URL points at `registry.npmjs.org`."""
    if not lockfile.is_file():
        return []
    try:
        doc = json.loads(lockfile.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        return [f"{_shown(lockfile)}: lockfile is not valid JSON: {e}"]

    offenders: list[str] = []
    packages = doc.get("packages") or {}
    for pkg_key, pkg in packages.items():
        if not isinstance(pkg, dict):
            continue
        resolved = pkg.get("resolved")
        if not resolved:
            # Workspace-root entry and some peer-only entries legitimately
            # omit `resolved`. The lockfile-spec contract is that any
            # actually-installed tarball has `resolved` populated.
            continue
        if not isinstance(resolved, str):
            offenders.append(
                f"{_shown(lockfile)}: `{pkg_key}` has non-string `resolved`: {resolved!r}"
            )
            continue
        # Strip scheme to extract host. Banning git+, file:, ssh: by
        # not matching the https:// + allowed-host prefix.
        if not resolved.startswith("https://"):
            offenders.append(
                f"{_shown(lockfile)}: `{pkg_key}` resolves to non-https URL {resolved!r}"
            )
            continue
        try:
            host = resolved[len("https://") :].split("/", 1)[0]
        except IndexError:
            offenders.append(
                f"{_shown(lockfile)}: `{pkg_key}` resolves to malformed URL {resolved!r}"
            )
            continue
        if host not in ALLOWED_RESOLVED_HOSTS:
            offenders.append(
                f"{_shown(lockfile)}: `{pkg_key}` resolves to non-allowlisted host {host!r} "
                f"(URL: {resolved})"
            )
    return offenders


def run_sweep(repo_root: Path) -> int:
    """Run all three checks. Returns 0 when clean; 1 on any offender."""
    any_offenders = False

    workflow_offenders = check_no_shai_hulud_workflow(repo_root)
    if workflow_offenders:
        any_offenders = True
        print(
            f"npm-ioc-sweep: check 1 (no `{SHAI_HULUD_WORKFLOW}`) — FAIL",
            file=sys.stderr,
        )
        for o in workflow_offenders:
            print(f"  - {o}", file=sys.stderr)
    else:
        print(f"npm-ioc-sweep: check 1 (no `{SHAI_HULUD_WORKFLOW}`) — OK")

    for lockfile_rel in LOCKFILE_PATHS:
        lockfile = repo_root / lockfile_rel
        try:
            lf_label = lockfile.relative_to(repo_root)
        except ValueError:
            lf_label = lockfile

        lifecycle_offenders = check_no_lifecycle_scripts(lockfile)
        if lifecycle_offenders:
            any_offenders = True
            print(
                f"npm-ioc-sweep: check 2 (no preinstall/postinstall in {lf_label}) — FAIL",
                file=sys.stderr,
            )
            for o in lifecycle_offenders:
                print(f"  - {o}", file=sys.stderr)
        else:
            print(f"npm-ioc-sweep: check 2 (no preinstall/postinstall in {lf_label}) — OK")

        host_offenders = check_lockfile_hosts(lockfile)
        if host_offenders:
            any_offenders = True
            print(
                f"npm-ioc-sweep: check 3 (registry.npmjs.org only in {lf_label}) — FAIL",
                file=sys.stderr,
            )
            for o in host_offenders:
                print(f"  - {o}", file=sys.stderr)
        else:
            print(f"npm-ioc-sweep: check 3 (registry.npmjs.org only in {lf_label}) — OK")

    return 1 if any_offenders else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help=f"Repo root to sweep (default: {REPO_ROOT})",
    )
    args = parser.parse_args()
    return run_sweep(args.repo_root.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
